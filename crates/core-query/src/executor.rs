use core_index::{
    analyzer::analyzer::{ Analyzer, NAME_KEY, QUALIFIED_NAME_KEY },
    fuzzy::{ FuzzyExpansion, FuzzyOptions, FuzzySpec, select_expansions },
    posting::{
        Posting,
        PostingCursor,
        PostingList,
        cursor::TermCursor,
        ops::{ intersection, union_many },
    },
    search::{ SearchIndex, SearchNumeric, SearchStats },
    types::{ ArrayRowId, DocId, XPathId, position_span, position_start },
};
use roaring::RoaringTreemap;
use std::{ borrow::Cow, collections::{ BinaryHeap, HashMap, hash_map::Entry } };

use ahash::{ HashSet, HashSetExt };

use core_protocol::command_reponse_definitions::{ Fuzziness, MatchMode };
use core_timing::timed;

use crate::{
    MatchedTerm,
    ScoredPosting,
    SearchHit,
    TopHit,
    ast::{ PhraseTerm, Query, SynonymAlternative },
    corpus_stats::{ CorpusStats, FieldTotals, FuzzyKey, LeafKey, ScoringStats },
    planner::{ RankPlan, Tier },
    prepared::PreparedQuery,
    rank_order,
    ranking::{ ANY_POOL_SIZE, MAX_CLAUSES, MIN_COVERAGE, POOL_SIZE, RankedGroup, finalize },
    resolver::{ FieldCtx, FieldQuery, SameElementBinding },
    scorer::{ bm25_idf, fuzzy_decay, score_term_into },
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
    /// True when every corrected word is clearly the best spelling: no closer or equally close
    /// word is anywhere near as common. Search replaces the query only then; otherwise the
    /// correction is offered as a suggestion.
    pub confident: bool,
}

impl DidYouMeanReport {
    pub fn empty(original: &str) -> Self {
        DidYouMeanReport {
            original: original.to_string(),
            corrected: String::new(),
            words: Vec::new(),
            confident: false,
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
    Docs(RoaringTreemap),
    Scored(Vec<ScoredPosting>),
}

// One leaf query read in one field: the key its statistics are filed under, the field it was
// looked up in (the exact field for an exact match) and the postings it found.
struct Leaf<'a> {
    key: LeafKey,
    xpath: XPathId,
    postings: Cow<'a, PostingList>,
}

enum ListSource<'a> {
    /// A list computed or already read (phrase, exact, prefix, a phrase form's words).
    Decoded(Cow<'a, PostingList>),
    /// A plain term in a field, opened as a cursor when it is used. `local` is how many
    /// documents of this index have it (metadata, nothing decoded).
    Term {
        term: String,
        xpath: XPathId,
        local: u32,
    },
}

impl ListSource<'_> {
    fn is_empty(&self) -> bool {
        match self {
            Self::Decoded(list) => list.is_empty(),
            Self::Term { local, .. } => *local == 0,
        }
    }

    /// A cursor over the list, with the largest weight in it.
    fn open<'s, I: SearchIndex>(&'s self, index: &'s I) -> (TermCursor<'s>, u16) {
        match self {
            Self::Decoded(list) => (TermCursor::Slice(PostingCursor::new(list)), list.max_weight()),
            Self::Term { term, xpath, .. } =>
                match index.term_cursor(term, *xpath) {
                    Some(info) => (info.cursor, info.max_weight),
                    None => (TermCursor::Empty, 0),
                }
        }
    }
}

// One form of a query clause read in one field: its lists with the document frequency each is
// scored with (see `wand::FormRef`).
struct FormLists<'a> {
    xpath: XPathId,
    lists: Vec<(ListSource<'a>, u32)>,
    weight: u16,
    any:bool,
     phrase: Option<Vec<u32>>,
}

impl FormLists<'_> {
    /// The form as WAND reads it, counting for searched field number `field`.
    fn as_ref<'s, I: SearchIndex>(&'s self, index: &'s I, field: usize) -> FormRef<'s> {
        FormRef {
            xpath: self.xpath,
            field,
            lists: self.lists
                .iter()
                .map(|(source, doc_freq)| {
                    let (cursor, max_weight) = source.open(index);
                    TermRef {
                        cursor,
                        doc_freq: *doc_freq,
                        max_weight,
                    }
                })
                .collect(),
            weight: self.weight,
            any:self.any,
            phrase: self.phrase.clone()
            
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
        if ctx.row_keyed { self.index.doc_of_row(ArrayRowId::from_raw(id)) } else { Some(id) }
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

    #[timed(search)]
    fn fetch_term_list(&self, term: &str, xpath: XPathId) -> (Cow<'a, PostingList>, u32) {
        let key = LeafKey::Term(term.to_string());
        let (postings, local_doc_freq) = if
            let Some(list) = self.prepared.and_then(|p| p.list(xpath, &key))
        {
            (Cow::Borrowed(list), list.len() as u32)
        } else {
            let found = self.index.lookup_term(term, xpath);
            (Cow::Owned(found.postings), found.doc_freq)
        };
        let doc_freq = self.leaf_doc_freq(xpath, &key, local_doc_freq as usize);
        (postings, doc_freq)
    }
    /// How many documents of this index have `term` in `xpath`, from metadata.
    fn local_doc_freq(&self, term: &str, xpath: XPathId) -> u32 {
        if term.is_empty() { 0 } else { self.index.doc_freq(term, xpath) }
    }

    /// The document frequency `term` is scored with: the whole search's when known.
    fn term_doc_freq(&self, term: &str, xpath: XPathId) -> u32 {
        let local = self.local_doc_freq(term, xpath);
        self.leaf_doc_freq(xpath, &LeafKey::Term(term.to_string()), local as usize)
    }

    /// `term` as a form list, read through a cursor when used.
    fn term_source(&self, term: &str, xpath: XPathId) -> (ListSource<'a>, u32) {
        let local = self.local_doc_freq(term, xpath);
        let doc_freq = self.leaf_doc_freq(xpath, &LeafKey::Term(term.to_string()), local as usize);
        (
            ListSource::Term {
                term: term.to_string(),
                xpath,
                local,
            },
            doc_freq,
        )
    }

    /// A cursor over `term` in `xpath`; `Empty` when no document has it.
    fn open_term_cursor(&self, term: &str, xpath: XPathId) -> TermCursor<'a> {
        if term.is_empty() {
            return TermCursor::Empty;
        }
        let index: &'a I = self.index;
        index.term_cursor(term, xpath).map_or(TermCursor::Empty, |info| info.cursor)
    }
    /// A cursor over `term` in `xpath`, with the document frequency it is scored with.
    fn open_term(&self, term: &str, xpath: XPathId) -> Option<TermRef<'a>> {
        if term.is_empty() {
            return None;
        }
        let index: &'a I = self.index;
        let info = index.term_cursor(term, xpath)?;
        let key = LeafKey::Term(term.to_string());
        let doc_freq = self.leaf_doc_freq(xpath, &key, info.doc_freq as usize);
        Some(TermRef {
            cursor: info.cursor,
            doc_freq,
            max_weight: info.max_weight,
        })
    }

    /// Cursors for every term in every field. With `require_all` (a conjunction) a field
    /// missing any term gets no terms, so it cannot match.
    fn open_field_terms(
        &self,
        terms: &[&str],
        xpaths: &[XPathId],
        require_all: bool
    ) -> Vec<FieldTerms<'a>> {
        xpaths
            .iter()
            .map(|&xpath| {
                let opened = terms.iter().map(|term| self.open_term(term, xpath));
                let terms = if require_all {
                    opened.collect::<Option<Vec<_>>>().unwrap_or_default()
                } else {
                    opened.flatten().collect()
                };
                FieldTerms { xpath, terms }
            })
            .collect()
    }

    //WAND/conjunctive fast path for flat term lists (skips the general traversal)
    fn execute_top_k_retrieval(
        &self,
        query: &Query,
        xpaths: &[XPathId],
        k: usize,
        restrict: Option<&RoaringTreemap>
    ) -> Option<Vec<WandHit>> {
        //disjunctive flat-term cases (Term / Wand / Or) -> additive WAND
        if let Some(terms) = additive_terms(query) {
            return Some(
                wand_top_k_multi_field(
                    &self.stats(),
                    self.open_field_terms(&terms, xpaths, false),
                    k,
                    restrict
                )
            );
        }

        //conjunctive flat-term case (And) -> conjunctive top-k
        if let Query::And(parts) = query && parts.iter().all(|p| matches!(p, Query::Term(_))) {
            let terms: Vec<&str> = parts
                .iter()
                .filter_map(|p| {
                    match p {
                        Query::Term(t) => Some(t.as_str()),
                        _ => None,
                    }
                })
                .collect();

            return Some(
                conjunctive_top_k_multi_field(
                    &self.stats(),
                    self.open_field_terms(&terms, xpaths, true),
                    k,
                    restrict
                )
            );
        }
        // fuzzy: one clause per typed word, its forms the word's expansions in every field,
        // each weighted down by its edits; a doc must hold every word
        if let Query::Fuzzy(raw, fuzziness, spec) = query {
            let words = self.fuzzy_words(raw);
            if words.is_empty() || words.len() >= MAX_FUZZY_WORDS {
                return Some(Vec::new());
            }

            let forms: Vec<Vec<(usize, FormLists<'a>)>> = words
                .iter()
                .map(|word| {
                    let opts = fuzzy_options(word, *fuzziness, *spec);
                    xpaths
                        .iter()
                        .enumerate()
                        .flat_map(|(field, &xpath)| {
                            let candidates = self
                                .fuzzy_candidates(word, xpath, opts)
                                .into_iter()
                                .map(|e| (e.term, e.edits, e.doc_freq));
                            select_expansions(word, candidates, spec.max_expansions)
                                .into_iter()
                                .map(move |expansion| {
                                    let local = self.local_doc_freq(&expansion.term, xpath);
                                    (
                                        field,
                                        FormLists {
                                            xpath,
                                            lists: vec![(
                                                ListSource::Term {
                                                    term: expansion.term,
                                                    xpath,
                                                    local,
                                                },
                                                expansion.doc_freq,
                                            )],
                                            weight: 1000 / (1 + u16::from(expansion.edits)),
                                            any:false,
                                            phrase: None
                                        },
                                    )
                                })
                        })
                        .collect()
                })
                .collect();

            // a word with no expansion in this index: no document holds every word
            if
                forms
                    .iter()
                    .any(|word_forms| word_forms.iter().all(|(_, form)| form.lists[0].0.is_empty()))
            {
                return Some(Vec::new());
            }

            let index: &'a I = self.index;
            let clauses = FieldClauses {
                clauses: forms
                    .iter()
                    .map(|word_forms| {
                        word_forms
                            .iter()
                            .map(|(field, form)| form.as_ref(index, *field))
                            .collect()
                    })
                    .collect(),
            };
            return Some(wand_top_k_grouped(&self.stats(), vec![clauses], k, Some(1.0), restrict));
        }
        None
    }

    fn execute_grouped_retrieval(
        &self,
        query: &Query,
        ctxs: &[FieldCtx],
        k: usize,
        restrict: Option<&RoaringTreemap>,
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
        let index: &'a I = self.index;
        let fields: Vec<FieldClauses<'_>> = lists
            .iter()
            .map(|clauses| FieldClauses {
                clauses: clauses
                    .iter()
                    .map(|forms| {
                        forms
                            .iter()
                            .map(|(field, form)| form.as_ref(index, *field))
                            .collect()
                    })
                    .collect(),
            })
            .collect();
        let stats = self.stats();
        Some(
            if conjunctive {
                conjunctive_top_k_grouped(&stats, fields, k, restrict)
            } else {
                wand_top_k_grouped(&stats, fields, k, coverage, restrict)
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
                 Query::Phrase(terms) => Some(FormLists {
                    xpath: ctx.xpath,
                    lists: terms.iter().map(|term| self.term_source(&term.text, ctx.xpath)).collect(),
                    weight: form.weight,
                    any: false,
                    phrase: Some(terms.iter().map(|term| term.offset).collect()),
                }),
                Query::Exact(_) => self.leaf_form(&form.query, ctx, None, form.weight),
                Query::And(words) => {
                    let lists: Option<Vec<(ListSource<'a>, u32)>> = words
                        .iter()
                        .map(|word| {
                            match word {
                                Query::Term(term) => Some(self.term_source(term, ctx.xpath)),
                                _ => {
                                    let leaf = self.leaf(word, ctx)?;
                                    let doc_freq = self.leaf_doc_freq(
                                        leaf.xpath,
                                        &leaf.key,
                                        leaf.postings.len()
                                    );
                                    Some((ListSource::Decoded(leaf.postings), doc_freq))
                                }
                            }
                        })
                        .collect();
                    lists.map(|lists| FormLists {
                        xpath: ctx.xpath,
                        lists,
                        weight: form.weight,
                        any:false,
                        phrase:None
                    })
                }
                Query::Wand(words) if words.iter().all(|word| matches!(word, Query::Term(_))) => {
                    let lists = words
                        .iter()
                        .filter_map(|word| match word {
                            Query::Term(term) => Some(self.term_source(term, ctx.xpath)),
                            _ => None,
                        })
                        .collect();
                    Some(FormLists {
                        xpath: ctx.xpath,
                        lists,
                        weight: form.weight,
                        any: true,
                        phrase:None
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
        let (xpath, source, own) = match query {
            Query::Term(term) => {
                let (source, own) = self.term_source(term, ctx.xpath);
                (ctx.xpath, source, own)
            }
            _ => {
                let leaf = self.leaf(query, ctx)?;
                let own = self.leaf_doc_freq(leaf.xpath, &leaf.key, leaf.postings.len());
                (leaf.xpath, ListSource::Decoded(leaf.postings), own)
            }
        };
        let doc_freq = min_doc_freq.map_or(own, |floor| own.max(floor));
        Some(FormLists {
            xpath,
            lists: vec![(source, doc_freq)],
            weight,
            any:false,
            phrase:None
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
    #[timed(search)]
    fn execute_phrase(&self, terms: &[PhraseTerm], xpath: XPathId) -> PostingList {
        if terms.is_empty() {
            return PostingList::default();
        }
        let mut cursors: Vec<TermCursor<'a>> = terms
            .iter()
            .map(|term| self.open_term_cursor(&term.text, xpath))
            .collect();
        if cursors.iter().any(TermCursor::is_exhausted) {
            return PostingList::default();
        }

        let offsets: Vec<u32> = terms
            .iter()
            .map(|term| term.offset)
            .collect();
        // walk the rarest word's documents: the cost is that of the rarest word, not the first
        let rarest = (0..terms.len())
            .min_by_key(|&i| self.local_doc_freq(&terms[i].text, xpath))
            .unwrap_or(0);

        let mut buffers: Vec<Vec<u32>> = vec![Vec::new(); terms.len()];
        let mut result = Vec::new();

        while let Some(doc_id) = cursors[rarest].doc_id() {
            // move every other word to this document; remember the furthest any of them went
            let mut skip_to = None;
            for (i, cursor) in cursors.iter_mut().enumerate() {
                if i == rarest {
                    continue;
                }
                cursor.advance_to(doc_id);
                match cursor.doc_id() {
                    Some(doc) if doc == doc_id => {}
                    Some(doc) => {
                        skip_to = Some(skip_to.map_or(doc, |known: DocId| known.max(doc)));
                    }
                    None => {
                        return PostingList::from_items(result);
                    }
                }
            }

            if let Some(target) = skip_to {
                cursors[rarest].advance_to(target);
                continue;
            }

            let read = cursors
                .iter()
                .zip(buffers.iter_mut())
                .all(|(cursor, buffer)| cursor.positions_into(buffer).is_ok());
            if read {
                let position_lists: Vec<&[u32]> = buffers.iter().map(Vec::as_slice).collect();
                if phrase_matches(&position_lists, &offsets) {
                    // the first word's posting, as before, so phrase scoring is unchanged
                    result.push(
                        Posting::with_weight(doc_id, buffers[0].clone(), cursors[0].weight())
                    );
                }
            }
            cursors[rarest].next();
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

    //sitaa funkcija darija kipa exact bet kip arii phrase kas nu nav pareizi, sim jabut exact,
    //jauana logika viss ko izdara ir nonem "xxx" -> xxx
    #[timed(search)]
    fn execute_exact(&self, raw: &str, xpath: XPathId) -> PostingList {
        let raw = raw.trim();

        // incase someone did phrase + exact we can be forgiving and drop the ""
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

        self.index.lookup(raw, xpath)
    }

    fn ascend(&self, rows: RoaringTreemap, levels: u32) -> RoaringTreemap {
        let mut rows = rows;
        for _ in 0..levels {
            rows = rows
                .into_iter()
                .filter_map(|raw| self.index.parent_of_row(ArrayRowId::from_raw(raw)))
                .map(|row| row.raw())
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
                        let mut out = RoaringTreemap::new();
                        for &ctx in ctxs {
                            out |= self.posting_ids(&self.execute_fuzzy(v, ctx.xpath, *fz, *spec));
                        }
                        EvalOutcome::Docs(out)
                    }
                    EvalMode::Rank => {
                        EvalOutcome::Scored(self.execute_scored_fuzzy(v, ctxs, *fz, *spec))
                    }
                }
            Query::Range(r) => {
                let mut out = RoaringTreemap::new();
                for &ctx in ctxs {
                    out |= self.index.numeric_range(ctx.xpath, r.lo, r.hi);
                }
                EvalOutcome::Docs(out)
            }
            Query::Bool(b) => {
                let mut out = RoaringTreemap::new();
                for &ctx in ctxs {
                    if *b {
                        out |= self.index.bool_true_ids(ctx.xpath);
                    } else {
                        out |= self.index.bool_false_ids(ctx.xpath);
                    }
                }
                EvalOutcome::Docs(out)
            }

            Query::Wand(parts) | Query::Or(parts) => self.eval_union(parts, ctxs, mode),
            Query::And(parts) => self.eval_and(parts, ctxs, mode),
            Query::Synonym(forms) => self.eval_synonym(forms, ctxs, mode),

            Query::SameElement(_) => EvalOutcome::Docs(RoaringTreemap::new()),

            // every live document (inside an AND, a NOT or a filter it means "no restriction")
            Query::MatchAll =>
                EvalOutcome::Docs(match self.index.doc_range() {
                    Some((min, max)) =>
                        (min..=max).filter(|id| !self.index.is_deleted(*id)).collect(),
                    None => RoaringTreemap::new(),
                }),
        }
    }

    #[timed(highlight)]
    pub fn matched_terms(
        &self,
        query: &Query,
        doc_id: DocId,
        ctxs: &[FieldCtx]
    ) -> Vec<MatchedTerm> {
        let mut out = Vec::new();
        self.collect_matched(query, doc_id, ctxs, &mut out);
        out
    }

    #[timed(highlight)]
    fn collect_matched(
        &self,
        query: &Query,
        doc_id: DocId,
        ctxs: &[FieldCtx],
        out: &mut Vec<MatchedTerm>
    ) {
        match query {
            Query::Term(_) | Query::Prefix(_) | Query::Wildcard(_) | Query::Exact(_) => {
                for &ctx in ctxs {
                    if ctx.row_keyed {
                        self.push_row_matches(query, doc_id, ctx, out);
                    } else if let Query::Term(term) = query {
                        let posting = self.posting_for(term, ctx.xpath, doc_id);
                        Self::push_leaf_matches(posting.as_ref(), ctx.xpath, out);
                    } else if let Some(leaf) = self.leaf(query, ctx) {
                        let items = leaf.postings.items();
                        let posting = items
                            .binary_search_by_key(&doc_id, |p| p.doc_id)
                            .ok()
                            .map(|index| &items[index]);
                        Self::push_leaf_matches(posting, ctx.xpath, out);
                    }
                }
            }
            Query::Phrase(terms) => {
                for &ctx in ctxs {
                    if ctx.row_keyed {
                        self.collect_phrase_row_keyed(terms, doc_id, ctx, out);
                    } else {
                        self.collect_phrase(terms, doc_id, ctx.xpath, out);
                    }
                }
            }

            Query::Fuzzy(raw, fuzziness, fuzzy_spec) => {
                for &ctx in ctxs {
                    if ctx.row_keyed {
                        let list = self.execute_fuzzy(raw, ctx.xpath, *fuzziness, *fuzzy_spec);
                        for posting in list.items() {
                            if
                                self.index.doc_of_row(ArrayRowId::from_raw(posting.doc_id)) ==
                                    Some(doc_id) &&
                                !posting.positions.is_empty()
                            {
                                out.push(MatchedTerm {
                                    xpath: ctx.xpath,
                                    row_id: Some(ArrayRowId::from_raw(posting.doc_id)),
                                    row_path: Vec::new(),
                                    positions: posting.positions.to_vec(),
                                    offsets: posting.offsets.to_vec(),
                                });
                            }
                        }
                    } else {
                        // the doc is already a hit: read each expansion for it alone
                        for word in self.fuzzy_words(raw) {
                            let opts = fuzzy_options(&word, *fuzziness, *fuzzy_spec);
                            let candidates = self
                                .fuzzy_candidates(&word, ctx.xpath, opts)
                                .into_iter()
                                .map(|e| (e.term, e.edits, e.doc_freq));
                            for expansion in select_expansions(
                                &word,
                                candidates,
                                fuzzy_spec.max_expansions
                            ) {
                                let posting = self.posting_for(&expansion.term, ctx.xpath, doc_id);
                                Self::push_leaf_matches(posting.as_ref(), ctx.xpath, out);
                            }
                        }
                    }
                }
            }
            | Query::Wand(children)
            | Query::And(children)
            | Query::Or(children)
            | Query::SameElement(children) => {
                for child in children {
                    self.collect_matched(child, doc_id, ctxs, out);
                }
            }
            Query::Synonym(forms) => {
                for form in forms {
                    self.collect_matched(&form.query, doc_id, ctxs, out);
                }
            }
            Query::Range(_) | Query::Bool(_) | Query::MatchAll => {}
        }
    }

    fn push_leaf_matches(posting: Option<&Posting>, xpath: XPathId, out: &mut Vec<MatchedTerm>) {
        if let Some(p) = posting {
            if !p.positions.is_empty() {
                //offsets migh be empty here, the re-analyzing and all the magic happens later in
                //highlight.rs
                out.push(MatchedTerm {
                    xpath,
                    row_id: None,
                    positions: p.positions.to_vec(),
                    row_path: Vec::new(),
                    offsets: p.offsets.to_vec(),
                });
            }
        }
    }

    //row-keyed postings have array_row_id we need to match it to the correct dovument
    #[timed(highlight)]
    fn push_row_matches(
        &self,
        query: &Query,
        doc_id: DocId,
        ctx: FieldCtx,
        out: &mut Vec<MatchedTerm>
    ) {
        let Some(leaf) = self.leaf(query, ctx) else {
            return;
        };
        for posting in leaf.postings.items() {
            if self.index.doc_of_row(ArrayRowId::from_raw(posting.doc_id)) == Some(doc_id) {
                if !posting.positions.is_empty() {
                    out.push(MatchedTerm {
                        xpath: ctx.xpath,
                        row_id: Some(ArrayRowId::from_raw(posting.doc_id)),
                        row_path: Vec::new(),
                        positions: posting.positions.to_vec(),
                        offsets: posting.offsets.to_vec(),
                    });
                }
            }
        }
    }

    //kind of stupid that we need this, but a phrase match needs to return offsets as a single byte span
    //not multiple ones
    fn push_phrase_matches(
        terms: &[PhraseTerm],
        xpath: XPathId,
        row_id: Option<ArrayRowId>,
        word_postings: &[Option<Posting>],
        out: &mut Vec<MatchedTerm>
    ) {
        if word_postings.iter().any(Option::is_none) {
            return;
        }
        let position_lists: Vec<&[u32]> = word_postings
            .iter()
            .map(|p| p.as_ref().unwrap().positions.as_slice())
            .collect();
        let phrase_offsets: Vec<u32> = terms
            .iter()
            .map(|t| t.offset)
            .collect();
        let has_offsets = !word_postings[0].as_ref().unwrap().offsets.is_empty();
        let last_idx = terms.len() - 1;

        for &first_pos in position_lists[0] {
            let mut matched: Vec<u32> = Vec::with_capacity(terms.len());
            matched.push(first_pos);
            let mut next = position_start(first_pos) + position_span(first_pos);
            let mut ok = true;

            for i in 1..terms.len() {
                let gap = phrase_offsets[i].saturating_sub(phrase_offsets[i - 1]).saturating_sub(1);
                let want = next + gap;
                let at = position_lists[i].partition_point(|&p| position_start(p) < want);
                match position_lists[i].get(at) {
                    Some(&p) if position_start(p) == want => {
                        matched.push(p);
                        next = want + position_span(p);
                    }
                    _ => {
                        ok = false;
                        break;
                    }
                }
            }

            if ok {
                if has_offsets {
                    let first_enc = matched[0];
                    let last_enc = *matched.last().unwrap();
                    let first = word_postings[0].as_ref().unwrap();
                    let last = word_postings[last_idx].as_ref().unwrap();
                    let (start, _) = Self::offset_for(
                        &first.positions,
                        &first.offsets,
                        position_start(first_enc)
                    ).unwrap_or((0, 0));
                    let (_, end) = Self::offset_for(
                        &last.positions,
                        &last.offsets,
                        position_start(last_enc)
                    ).unwrap_or((0, 0));
                    out.push(MatchedTerm {
                        xpath,
                        positions: Vec::new(),
                        row_path: Vec::new(),
                        row_id,
                        offsets: vec![(start, end)],
                    });
                } else {
                    out.push(MatchedTerm {
                        xpath,
                        row_id,
                        row_path: Vec::new(),
                        positions: matched,
                        offsets: Vec::new(),
                    });
                }
            }
        }
    }

    #[timed(highlight)]
    fn collect_phrase(
        &self,
        terms: &[PhraseTerm],
        doc_id: DocId,
        xpath: XPathId,
        out: &mut Vec<MatchedTerm>
    ) {
        if terms.is_empty() {
            return;
        }
        let word_postings: Vec<Option<Posting>> = terms
            .iter()
            .map(|t| self.posting_for(&t.text, xpath, doc_id))
            .collect();
        Self::push_phrase_matches(terms, xpath, None, &word_postings, out);
    }

    fn collect_phrase_row_keyed(
        &self,
        terms: &[PhraseTerm],
        doc_id: DocId,
        ctx: FieldCtx,
        out: &mut Vec<MatchedTerm>
    ) {
        if terms.is_empty() {
            return;
        }
        let xpath = ctx.xpath;
        let lists: Vec<Cow<'a, PostingList>> = terms
            .iter()
            .map(|t| self.fetch_term_list(&t.text, xpath).0)
            .collect();
        for posting in lists[0].items() {
            if self.index.doc_of_row(ArrayRowId::from_raw(posting.doc_id)) != Some(doc_id) {
                continue;
            }
            let row_id = posting.doc_id;
            let word_postings: Vec<Option<Posting>> = lists
                .iter()
                .map(|l| {
                    let items = l.items();
                    items
                        .binary_search_by_key(&row_id, |p| p.doc_id)
                        .ok()
                        .map(|index| items[index].clone())
                })

                .collect();
            Self::push_phrase_matches(
                terms,
                xpath,
                Some(ArrayRowId::from_raw(row_id)),
                &word_postings,
                out
            );
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
                let mut out = RoaringTreemap::new();
                for &ctx in ctxs {
                    if let Some(leaf) = self.leaf(query, ctx) {
                        out |= self.posting_ids(&leaf.postings);
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
            let mut out = RoaringTreemap::new();
            for form in forms {
                if let EvalOutcome::Docs(docs) = self.evaluate(&form.query, ctxs, EvalMode::Filter) {
                    out |= docs;
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

    fn posting_ids(&self, postings: &PostingList) -> RoaringTreemap {
        postings
            .items()
            .iter()
            .map(|p| p.doc_id)
            .collect()
    }

    fn eval_union(&self, parts: &[Query], ctxs: &[FieldCtx], mode: EvalMode) -> EvalOutcome {
        match mode {
            EvalMode::Filter => {
                let mut out = RoaringTreemap::new();
                for part in parts {
                    if let EvalOutcome::Docs(docs) = self.evaluate(part, ctxs, EvalMode::Filter) {
                        out |= docs;
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
                let mut candidates: Option<RoaringTreemap> = None;
                for part in parts {
                    if let EvalOutcome::Docs(docs) = self.evaluate(part, ctxs, EvalMode::Filter) {
                        candidates = Some(match candidates {
                            Some(cur) => cur & docs,
                            None => docs,
                        });
                        if candidates.as_ref().is_some_and(RoaringTreemap::is_empty) {
                            return EvalOutcome::Docs(RoaringTreemap::new());
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
    ) -> RoaringTreemap {
        let mut rows: Option<RoaringTreemap> = None;
        for (child, clause) in children.iter().zip(&binding.clauses) {
            let r: RoaringTreemap = match &clause.nested {
                Some(nested) => {
                    let Query::SameElement(inner_children) = child else {
                        continue;
                    };
                    self.same_element_rows(inner_children, nested)
                }
                None =>
                    match self.evaluate(child, &[clause.ctx], EvalMode::Filter) {
                        EvalOutcome::Docs(d) => d,
                        _ => RoaringTreemap::new(),
                    }
            };
            let r = self.ascend(r, clause.depth.saturating_sub(binding.depth));
            rows = Some(match rows {
                Some(cur) => cur & r,
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
            // the grouped (WAND) path reads synonym phrase forms through cursors itself
            let lazy_phrases = ctxs.iter().all(|ctx| !ctx.row_keyed)
                && grouped_parts(query, false).is_some();
            for &ctx in ctxs {
                self.prepare_leaves(query, ctx, &mut prepared, lazy_phrases);
            }
        }
        prepared
    }

    fn prepare_leaves(&self, query: &Query, ctx: FieldCtx, out: &mut PreparedQuery, lazy_phrases:bool) {
        match query {
            Query::Wand(parts) | Query::Or(parts) | Query::And(parts) => {
                for part in parts {
                    self.prepare_leaves(part, ctx, out,lazy_phrases);
                }
            }
            Query::Synonym(forms) => {
                for form in forms {
                    match &form.query {
                        // WAND checks this phrase on positions itself: only its words' statistics
                        Query::Phrase(terms) if lazy_phrases => {
                            for term in terms {
                                self.prepare_leaves(
                                    &Query::Term(term.text.clone()),
                                    ctx,
                                    out,
                                    lazy_phrases,
                                );
                            }
                        }
                        other => self.prepare_leaves(other, ctx, out, lazy_phrases),
                    }
                }
            }

            Query::Phrase(terms) => {
                // phrase forms are also scored by their words (score_phrase_as_words)
                for term in terms {
                    self.prepare_leaves(&Query::Term(term.text.clone()), ctx, out,lazy_phrases);
                }
                let Some((key, xpath)) = Self::leaf_key(query, ctx) else {
                    return;
                };
                if out.has_leaf(xpath, &key) {
                    return;
                }
                out.insert_field(xpath, self.field_totals(xpath));
                let postings = self.fetch_leaf(query, xpath);
                out.insert_leaf(xpath, key, postings);
            }
            Query::Fuzzy(raw, fuzziness, spec) => {
                let xpath = ctx.xpath;
                out.insert_field(xpath, self.field_totals(xpath));
                for word in self.fuzzy_words(raw) {
                    let opts = fuzzy_options(&word, *fuzziness, *spec);
                    let key = FuzzyKey::new(&word, opts);
                    // the tiers of a plan share their words: collect each word's candidates once
                    if out.has_fuzzy(xpath, &key) {
                        continue;
                    }
                    // every candidate with this shard's document count; selection happens after the
                    // manager has summed the counts of all shards
                    let candidates = self.index.fuzzy_expansions(&word, xpath, opts);
                    out.insert_fuzzy(xpath, key, candidates);
                }
            }
            Query::Term(term) => {
                let xpath = ctx.xpath;
                let key = LeafKey::Term(term.clone());
                if out.has_leaf(xpath, &key) {
                    return;
                }
                out.insert_field(xpath, self.field_totals(xpath));
                // metadata only: WAND opens its own cursor, so the list is not decoded here
                let doc_freq: u32 = if term.is_empty() {
                    0
                } else {
                    self.index.doc_freq(term, xpath)
                };
                out.insert_doc_freq(xpath, key, u64::from(doc_freq));
            }
            _ => {
                let Some((key, xpath)) = Self::leaf_key(query, ctx) else {
                    return;
                };
                // the tiers of a plan share their words: read each list once
                if out.has_leaf(xpath, &key) {
                    return;
                }
                out.insert_field(xpath, self.field_totals(xpath));
                let postings = self.fetch_leaf(query, xpath);
                out.insert_leaf(xpath, key, postings);
            }
        }
    }

    #[timed(search)]
    pub fn filter_doc_ids(&self, filters: &HashMap<String, FieldQuery>) -> Option<RoaringTreemap> {
        if filters.is_empty() {
            return None;
        }

        let mut restrict: Option<RoaringTreemap> = None;
        for fq in filters.values() {
            let matched: RoaringTreemap = match &fq.query {
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
                Some(cur) => cur & matched,
                None => matched,
            });
            if restrict.as_ref().is_some_and(RoaringTreemap::is_empty) {
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
        restrict: Option<&RoaringTreemap>,
        match_mode: MatchMode // plan: RankPlan
    ) -> Vec<SearchHit> {
        if
            k == 0 ||
            restrict.is_some_and(RoaringTreemap::is_empty) ||
            self.index.doc_range().is_none()
        {
            return Vec::new();
        }

        //no query
        let Some(query) = query else {
            //filter-only search
            return match restrict {
                Some(allowed) =>
                    top_k_from_hits(
                        allowed.iter().map(|doc_id| SearchHit {
                            doc_id,
                            matched_terms: 0,
                            weight_sum: 0,
                            distance_factor: 1.0,
                            score: 0.0,
                            tier: 0,
                            ..SearchHit::default()
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
                        allowed.iter().map(|doc_id| SearchHit {
                            doc_id,
                            matched_terms: 0,
                            weight_sum: 0,
                            distance_factor: 1.0,
                            score: 0.0,
                            tier: 0,
                            ..SearchHit::default()
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
                                        ..SearchHit::default()
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
                if
                    ctxs.iter().all(|ctx| !ctx.row_keyed) &&
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
                return top_k_from_hits(self.cross_field_hits(query, ctxs, restrict), k);
            }
        }

        top_k_from_hits(by_doc.into_values(), k)
    }

    /// Every document `query` matches across the fields, scored (the `CrossFields` ranking before
    /// it is cut to k), so the cascade can take several tiers' top hits from one evaluation.
    fn cross_field_hits(
        &self,
        query: &Query,
        ctxs: &[FieldCtx],
        restrict: Option<&RoaringTreemap>
    ) -> Vec<SearchHit> {
        let EvalOutcome::Scored(scored) = self.evaluate(query, ctxs, EvalMode::Rank) else {
            return Vec::new();
        };
        let mut by_doc: HashMap<DocId, SearchHit> = HashMap::with_capacity(scored.len());
        for p in scored {
            if !restrict.is_none_or(|a| a.contains(p.doc_id)) {
                continue;
            }
            let hit = SearchHit {
                doc_id: p.doc_id,
                matched_terms: p.matched_terms,
                weight_sum: (p.score / 1000).min(u64::from(u32::MAX)) as u32,
                distance_factor: p.density,
                score: ((p.score as f32) / 1000.0) * p.density,
                tier: 0,
                ..SearchHit::default()
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

    /// Runs the plan's tiers. The name and exact tiers are hard tiers: what the query names
    /// comes first. Every other kind (phrase, fuzzy, all, all-but-one, any) is a source of
    /// candidates for the one relevance tier, where every hit is measured the same way: its raw
    /// BM25, the clauses it matches, how close its words are and how much of the query its name
    /// holds. `ranking::finalize` turns that into the score. Tiers are positions in the
    /// configured order (`plan.ranks`), so every group and shard numbers them the same.
    #[timed(search)]
    fn rank_cascade(
        &self,
        plan: &RankPlan,
        ctxs: &[FieldCtx],
        k: usize,
        restrict: Option<&RoaringTreemap>,
        match_mode: MatchMode
    ) -> RankedGroup {
        let plain_xpaths: Vec<XPathId> = ctxs
            .iter()
            .filter(|ctx| !ctx.row_keyed)
            .map(|ctx| ctx.xpath)
            .collect();
        // under CrossFields a structured query is evaluated once (every match, scored) and each
        // source takes its own top hits from that. Not for flat queries: `rank_retrieve` ranks
        // those one document at a time, faster than scoring every match even once
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
        // the ranked pool: for a flat query of several words over plain fields (cross_fields),
        // the documents covering enough of the query's weight, in one WAND walk
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
            allowed: Option<&RoaringTreemap>
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
                                    .filter(|hit| allowed.contains(hit.doc_id))
                                    .cloned(),
                                fetch
                            ),
                        None => top_k_from_hits(all.iter().cloned(), fetch),
                    }
                }
                _ => self.rank_retrieve(Some(query), ctxs, fetch, allowed.or(restrict), match_mode),
            }
        };

        // every document once, at its best (tier, level), with its highest raw score
        let keep = |found: &mut HashMap<DocId, SearchHit>, mut hit: SearchHit| {
            hit.base = hit.score;
            match found.entry(hit.doc_id) {
                Entry::Occupied(mut slot) => {
                    let known = slot.get_mut();
                    if (hit.tier, hit.level) < (known.tier, known.level) {
                        known.tier = hit.tier;
                        known.level = hit.level;
                    }
                    known.base = known.base.max(hit.base);
                    known.score = known.base;
                    known.name_share = known.name_share.max(hit.name_share);
                    known.matched_terms = known.matched_terms.max(hit.matched_terms);
                }
                Entry::Vacant(slot) => {
                    slot.insert(hit);
                }
            }
        };
        let mut found: HashMap<DocId, SearchHit> = HashMap::new();

        // hard tiers: what the query names
        let mut named_count = 0usize;
        for tier in &plan.tiers {
            let fetch = k.saturating_sub(named_count);
            if fetch == 0 {
                break;
            }
            match tier {
                Tier::Name { keys, score_with } => {
                    let mut named: HashSet<DocId> = HashSet::new();
                    for (level, qualified) in [
                        (0u8, false),
                        (1u8, true),
                    ] {
                        let docs = self.name_docs(keys, ctxs, qualified);
                        let fresh: RoaringTreemap = docs
                            .into_iter()
                            .filter(|doc| {
                                !named.contains(doc) &&
                                    restrict.is_none_or(|limit| limit.contains(*doc))
                            })
                            .collect();
                        if fresh.is_empty() {
                            continue;
                        }
                        named.extend(fresh.iter());
                        let wanted = fresh.len().min(fetch as u64);
                        let mut level_hits = ranked(score_with, wanted as usize, Some(&fresh));
                        if level_hits.len() < (wanted as usize) {
                            let scored: HashSet<DocId> = level_hits
                                .iter()
                                .map(|hit| hit.doc_id)
                                .collect();
                            let mut rest: Vec<DocId> = fresh
                                .iter()
                                .filter(|doc| !scored.contains(doc))
                                .collect();
                            rest.sort_unstable();
                            rest.truncate(((wanted as usize) - level_hits.len()) as usize);
                            level_hits.extend(
                                rest.into_iter().map(|doc_id| SearchHit {
                                    doc_id,
                                    ..SearchHit::default()
                                })
                            );
                        }
                        for mut hit in level_hits {
                            hit.tier = plan.ranks.name;
                            hit.level = level;
                            hit.name_share = 1.0;
                            keep(&mut found, hit);
                        }
                    }
                }
                Tier::Exact(query) => {
                    for mut hit in ranked(query, fetch, None) {
                        hit.tier = plan.ranks.exact;
                        hit.level = 0;
                        hit.name_share = 1.0;
                        keep(&mut found, hit);
                    }
                }
                _ => {
                    continue;
                }
            }
            named_count = found.len();
        }

        // when the hard tiers already fill the page and come first, nothing the relevance
        // sources find could be shown
        let below_hard = plan.tiers.iter().all(|tier| {
            match tier {
                Tier::Name { .. } => plan.ranks.name < plan.ranks.relevance,
                Tier::Exact(_) => plan.ranks.exact < plan.ranks.relevance,
                _ => true,
            }
        });
        let filled = named_count >= k || (plan.stop_after > 0 && named_count >= plan.stop_after);
        let has_sources = plan.tiers
            .iter()
            .any(|tier| !matches!(tier, Tier::Name { .. } | Tier::Exact(_)));

        if has_sources && !(filled && below_hard) {
            // every document already found may come back from a source too
            let fetch = k.saturating_add(found.len());
            let mut relevance: HashMap<DocId, SearchHit> = HashMap::new();
            for tier in &plan.tiers {
                let hits = match tier {
                    Tier::Name { .. } | Tier::Exact(_) => {
                        continue;
                    }
                    Tier::All(_) if pool_parts.is_some() => {
                        let pool_query = Query::Wand(pool_parts.cloned().unwrap_or_default());
                        // the pool admits partial matches only when the plan has a source for them
                        let partial_allowed = plan.tiers
                            .iter()
                            .any(|tier| matches!(tier, Tier::AllButOne(_) | Tier::Any(_)));
                        let min_coverage = if partial_allowed { MIN_COVERAGE } else { 1.0 };
                        self.execute_grouped_retrieval(
                            &pool_query,
                            ctxs,
                            fetch.max(POOL_SIZE),
                            restrict,
                            true,
                            Some(min_coverage)
                        )
                            .map(|hits| hits.into_iter().map(wand_hit_to_search_hit).collect())
                            .unwrap_or_default()
                    }
                    // the pool already admits documents missing a word
                    Tier::AllButOne(_) if pool_parts.is_some() => {
                        continue;
                    }
                    Tier::AllButOne(Query::Wand(parts)) => {
                        // an OR of the words' ANDs, each with one word left out
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
                    Tier::Phrase { terms, score_with } => {
                        let matched = self.phrase_docs(terms, ctxs);
                        let allowed: RoaringTreemap = match restrict {
                            Some(limit) => {
                                matched
                                    .iter()
                                    .filter(|doc| limit.contains(**doc))
                                    .collect()
                            }
                            None => matched.into_iter().collect(),
                        };
                        if allowed.is_empty() {
                            continue;
                        }
                        ranked(score_with, fetch, Some(&allowed))
                    }
                    Tier::Fuzzy(Query::Fuzzy(raw, ..)) if
                        self.all_words_known(raw, &plain_xpaths)
                    => {
                        continue;
                    }
                    // a larger pool, so pages with more words survive until they are scored
                    Tier::Any(query) => ranked(query, fetch.max(ANY_POOL_SIZE), None),
                    Tier::All(query) | Tier::AllButOne(query) | Tier::Fuzzy(query) => {
                        ranked(query, fetch, None)
                    }
                };
                for mut hit in hits {
                    hit.tier = plan.ranks.relevance;
                    hit.level = 0;
                    keep(&mut relevance, hit);
                }
            }

            // A document whose name is a run of the query's words is what the query is about:
            // "Vera Wang was born in March" is about Vera Wang. Every candidate is checked,
            // wherever it came from, and named documents no source found are added.
            let shares = self.name_shares(plan, ctxs, restrict, relevance.keys().copied());
            let missing: RoaringTreemap = shares
                .keys()
                .copied()
                .filter(|doc| !relevance.contains_key(doc))
                .collect();
            if !missing.is_empty() {
                let scored: Vec<SearchHit> = match pool_parts {
                    Some(parts) =>
                        self
                            .execute_grouped_retrieval(
                                &Query::Wand(parts.clone()),
                                ctxs,
                                missing.len() as usize,
                                Some(&missing),
                                true,
                                None
                            )
                            .map(|hits| hits.into_iter().map(wand_hit_to_search_hit).collect())
                            .unwrap_or_default(),
                    None if plan.clauses.len() >= 2 =>
                        ranked(
                            &Query::Wand(plan.clauses.clone()),
                            missing.len() as usize,
                            Some(&missing)
                        ),
                    None =>
                        match plan.clauses.first() {
                            Some(clause) => ranked(clause, missing.len() as usize, Some(&missing)),
                            None => Vec::new(),
                        }
                };
                let scored_docs: HashSet<DocId> = scored
                    .iter()
                    .map(|hit| hit.doc_id)
                    .collect();
                let unscored = missing
                    .iter()
                    .filter(|doc| !scored_docs.contains(doc))
                    .map(|doc_id| SearchHit {
                        doc_id,
                        ..SearchHit::default()
                    });
                for mut hit in scored.into_iter().chain(unscored) {
                    hit.tier = plan.ranks.relevance;
                    hit.level = 0;
                    keep(&mut relevance, hit);
                }
            }
            for (doc, mut hit) in relevance {
                hit.name_share = shares.get(&doc).copied().unwrap_or(0.0);
                keep(&mut found, hit);
            }
        }
        
        let mut hits: Vec<SearchHit> = found.into_values().collect();
        let clause_weights = self.clause_masks(&plan.clauses, ctxs, &mut hits);
        self.apply_density(&mut hits, &plan.terms, &plain_xpaths);
        RankedGroup {
            hits,
            clause_weights,
        }
    }
    /// Sets each hit's `mask` (the clauses it matches in some plain field, by any form) and
    /// returns each clause's weight: the idf of its form in the field most documents have (the
    /// lowest there), as WAND weighs coverage. A clause no document has weighs 0, so an unknown
    /// word never counts against coverage. Empty when there is nothing to measure.
    #[timed(search)]
    fn name_shares(
        &self,
        plan: &RankPlan,
        ctxs: &[FieldCtx],
        restrict: Option<&RoaringTreemap>,
        candidates: impl Iterator<Item = DocId>
    ) -> HashMap<DocId, f32> {
        let mut shares: HashMap<DocId, f32> = HashMap::new();
        if plan.words.is_empty() {
            return shares;
        }
        let stats = self.stats();
        let weight_of = |word: &str| -> f32 {
            ctxs.iter()
                .filter(|ctx| !ctx.row_keyed)
                .filter_map(|ctx| {
                    let doc_freq = self.term_doc_freq(word, ctx.xpath);
                    let docs = stats.doc_count(ctx.xpath);
                    (doc_freq > 0 && docs > 0).then(|| bm25_idf(doc_freq, docs))
                })
                .reduce(f32::min)
                .unwrap_or(0.0)
        };
        let mut weights: HashMap<&str, f32> = plan.words
            .iter()
            .map(|word| (word.as_str(), weight_of(word)))
            .collect();
        // a word no document has (a typo nothing corrected) still is part of the query: it
        // weighs as much as the rarest known word, so a name holding only the others is partial
        let rarest = weights.values().copied().fold(0.0f32, f32::max);
        for weight in weights.values_mut() {
            if *weight == 0.0 {
                *weight = rarest;
            }
        }
        let total: f32 = plan.words
            .iter()
            .map(|word| weights[word.as_str()])
            .sum();
        if total > 0.0 {
            for key in &plan.named_runs {
                let held: f32 = key
                    .split(' ')
                    .filter_map(|word| weights.get(word))
                    .sum();
                let share = (held / total).min(1.0);
                for qualified in [false, true] {
                    for doc in self.name_docs(std::slice::from_ref(key), ctxs, qualified) {
                        if restrict.as_ref().is_none_or(|limit| limit.contains(doc)) {
                            let best = shares.entry(doc).or_insert(0.0);
                            *best = best.max(share);
                        }
                    }
                }
            }
        }

        shares
    }
    #[timed(search)]
    fn clause_masks(
        &self,
        clauses: &[Query],
        ctxs: &[FieldCtx],
        hits: &mut [SearchHit]
    ) -> Vec<f32> {
        for hit in hits.iter_mut() {
            hit.mask = 0;
        }
        if clauses.is_empty() || clauses.len() > MAX_CLAUSES {
            return Vec::new();
        }
        let stats = self.stats();
        let mut weights = Vec::with_capacity(clauses.len());
        let mut clause_forms: Vec<Vec<FormLists<'a>>> = Vec::with_capacity(clauses.len());
        for clause in clauses {
            let forms: Vec<FormLists<'a>> = ctxs
                .iter()
                .filter(|ctx| !ctx.row_keyed)
                .flat_map(|&ctx| self.clause_forms(clause, ctx))
                .filter(|form| {
                    !form.lists.is_empty()
                        && if form.any {
                            form.lists.iter().any(|(list, _)| !list.is_empty())
                        } else {
                            form.lists.iter().all(|(list, _)| !list.is_empty())
                        }
                })
                .collect();
            let largest = forms
                .iter()
                .map(|form| stats.doc_count(form.xpath))
                .max()
                .unwrap_or(0);
            let weight = forms
                .iter()
                .filter(|form| stats.doc_count(form.xpath) == largest)
                .map(|form| {
                    form.lists
                        .iter()
                        .map(|(_, doc_freq)| bm25_idf(*doc_freq, largest))
                        .sum::<f32>()
                })
                .reduce(f32::min)
                .unwrap_or(0.0);
            weights.push(weight);
            clause_forms.push(forms);
        }
        let mut order: Vec<usize> = (0..hits.len()).collect();
        order.sort_unstable_by_key(|&i| hits[i].doc_id);

        let index: &'a I = self.index;
                for (clause_index, forms) in clause_forms.iter().enumerate() {
                        let mut cursors: Vec<(bool, Option<&Vec<u32>>, Vec<TermCursor<'_>>)> = forms
                .iter()
                .map(|form| {
                    (
                        form.any,
                        form.phrase.as_ref(),
                        form.lists.iter().map(|(source, _)| source.open(index).0).collect(),
                    )
                })
                .collect();
            for &i in &order {
                let doc = hits[i].doc_id;
                let on = |cursor: &mut TermCursor<'_>| {
                    cursor.advance_to(doc);
                    cursor.doc_id() == Some(doc)
                };
                let matched = cursors.iter_mut().any(|(any, phrase, form)| {
                    if *any {
                        return form.iter_mut().any(on);
                    }
                    if !form.iter_mut().all(on) {
                        return false;
                    }
                    let Some(offsets) = phrase else { return true };
                    let mut buffers: Vec<Vec<u32>> = vec![Vec::new(); form.len()];
                    let read = form
                        .iter()
                        .zip(buffers.iter_mut())
                        .all(|(cursor, buffer)| cursor.positions_into(buffer).is_ok());
                    let lists: Vec<&[u32]> = buffers.iter().map(Vec::as_slice).collect();
                    read && phrase_matches(&lists, offsets)
                });
                if matched {
                    hits[i].mask |= 1u64 << clause_index;
                }
            }
        }
        for hit in hits.iter_mut() {
            hit.matched_terms = hit.mask.count_ones() as usize;
        }

        weights
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
                let doc_freq = self.term_doc_freq(&term.text, ctx.xpath);
                let kept = postings_of_cursor(
                    self.open_term_cursor(&term.text, ctx.xpath),
                    &matched
                );
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
                let kept = postings_of_cursor(self.open_term_cursor(&term.text, ctx.xpath), &docs);
                (ListSource::Decoded(Cow::Owned(kept)), self.term_doc_freq(&term.text, ctx.xpath))
            })
            .collect();
        Some(FormLists {
            xpath: ctx.xpath,
            lists,
            weight,
            any:false,
            phrase:None
        })
    }

    /// Documents containing the phrase in at least one plain field.
    fn phrase_docs(&self, terms: &[PhraseTerm], ctxs: &[FieldCtx]) -> HashSet<DocId> {
        let phrase = Query::Phrase(terms.to_vec());
        let mut docs = HashSet::new();
        for &ctx in ctxs.iter().filter(|ctx| !ctx.row_keyed) {
            if let Some(leaf) = self.leaf(&phrase, ctx) {
                docs.extend(
                    leaf.postings
                        .items()
                        .iter()
                        .map(|p| p.doc_id)
                );
            }
        }
        docs
    }
    /// Multiplies each hit's score by 1 + `DENSITY_BOOST` × how close together its words are, in
    /// its best field: the span of the words as typed against the span they take in the field
    /// (1 when they form the phrase), times the share of the query's words the field holds, so
    /// a page with two of four words side by side gets less than one with all four. A field
    /// needs at least two of the words to count; hits without one keep their score.
    fn apply_density(&self, hits: &mut [SearchHit], terms: &[PhraseTerm], xpaths: &[XPathId]) {
        for hit in hits.iter_mut() {
            hit.distance_factor = 0.0;
        }

        if terms.len() < 2 || hits.is_empty() {
            return;
        }

        // hits in doc id order, so every cursor only moves forward
        let mut order: Vec<usize> = (0..hits.len()).collect();
        order.sort_unstable_by_key(|&i| hits[i].doc_id);

        // every word's cursor in every field
        let mut fields: Vec<Vec<TermCursor<'a>>> = xpaths
            .iter()
            .map(|&xpath| {
                terms
                    .iter()
                    .map(|term| self.open_term_cursor(&term.text, xpath))
                    .collect()
            })
            .collect();

        let mut buffers: Vec<Vec<u32>> = vec![Vec::new(); terms.len()];
        let mut held: Vec<usize> = Vec::with_capacity(terms.len());

        for &i in &order {
            let doc = hits[i].doc_id;
            let mut best: Option<f32> = None;
            for cursors in &mut fields {
                // the words this field of the document holds, in the order they were typed
                held.clear();
                for (t, cursor) in cursors.iter_mut().enumerate() {
                    cursor.advance_to(doc);
                    if
                        cursor.doc_id() == Some(doc) &&
                        cursor.positions_into(&mut buffers[t]).is_ok()
                    {
                        held.push(t);
                    }
                }
                if held.len() < 2 {
                    continue;
                }
                let positions: Vec<&[u32]> = held
                    .iter()
                    .map(|&t| buffers[t].as_slice())
                    .collect();
                let Some(span) = min_cover_span(&positions) else {
                    continue;
                };
                let offsets: Vec<u32> = held
                    .iter()
                    .map(|&t| terms[t].offset)
                    .collect();
                // the span of these words as typed, stopword gaps included ("king of the hill" -> 4)
                let first = offsets.iter().copied().min().unwrap_or(0);
                let last = offsets.iter().copied().max().unwrap_or(0);
                let ideal = (last - first).saturating_add(1);
                let mut closeness = ((ideal as f32) / (span.max(1) as f32)).min(1.0);
                // full credit only for the words in the order they were typed
                if !phrase_matches(&positions, &offsets) {
                    closeness *= OUT_OF_ORDER;
                }
                let density = (closeness * (held.len() as f32)) / (terms.len() as f32);
                best = Some(best.map_or(density, |known| known.max(density)));
            }
            hits[i].distance_factor = best.unwrap_or(0.0);
        }
    }
    pub fn rank(
        &self,
        query: Option<&Query>,
        ctxs: &[FieldCtx],
        k: usize,
        restrict: Option<&RoaringTreemap>,
        match_mode: MatchMode,
        plan: &RankPlan
    ) -> Vec<SearchHit> {
        finalize(vec![self.rank_features(query, ctxs, k, restrict, match_mode, plan)], k)
    }
    /// The candidates of one analyzer group with their raw evidence, not yet scored: several
    /// groups are merged by `ranking::finalize` before anything is scored.
    pub fn rank_features(
        &self,
        query: Option<&Query>,
        ctxs: &[FieldCtx],
        k: usize,
        restrict: Option<&RoaringTreemap>,
        match_mode: MatchMode,
        plan: &RankPlan
    ) -> RankedGroup {
        if plan.tiers.is_empty() {
            let hits = self
                .rank_retrieve(query, ctxs, k, restrict, match_mode)
                .into_iter()
                .map(|mut hit| {
                    hit.base = hit.score;
                    hit.distance_factor = 0.0;
                    hit.mask = 0;
                    hit.name_share = 0.0;
                    hit
                })
                .collect();
            return RankedGroup {
                hits,
                clause_weights: Vec::new(),
            };
        }
        self.rank_cascade(plan, ctxs, k, restrict, match_mode)
    }

    fn rank_single_field(
        &self,
        query: &Query,
        ctx: FieldCtx,
        k: usize,
        restrict: Option<&RoaringTreemap>,
        by_doc: &mut HashMap<DocId, SearchHit>
    ) {
        if
            !ctx.row_keyed &&
            let Some(hits) = self.execute_top_k_retrieval(query, &[ctx.xpath], k, restrict)
        {
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

        let EvalOutcome::Scored(scored) = self.evaluate(query, &[ctx], EvalMode::Rank) else {
            return;
        };

        for p in scored {
            if !restrict.as_ref().is_none_or(|a| a.contains(p.doc_id)) {
                continue;
            }
            let hit = SearchHit {
                doc_id: p.doc_id,
                matched_terms: p.matched_terms,
                weight_sum: (p.score / 1000).min(u64::from(u32::MAX)) as u32,
                distance_factor: p.density,
                score: ((p.score as f32) / 1000.0) * p.density,
                tier: 0,
                ..SearchHit::default()
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

    fn all_words_known(&self, raw: &str, xpaths: &[XPathId]) -> bool {
        self.fuzzy_words(raw)
            .iter()
            .all(|word| xpaths.iter().any(|&xpath| self.term_doc_freq(word, xpath) > 0))
    }

    //helpers for highlights
    /// `term`'s posting for one document, read through a cursor: only the block holding the
    /// document is decoded, and only that document's positions and offsets are read.
    fn posting_for(&self, term: &str, xpath: XPathId, doc_id: DocId) -> Option<Posting> {
        let mut cursor = self.open_term_cursor(term, xpath);
        cursor.advance_to(doc_id);
        if cursor.doc_id() != Some(doc_id) {
            return None;
        }
        let mut positions = Vec::new();
        let mut offsets = Vec::new();
        cursor.positions_into(&mut positions).ok()?;
        cursor.offsets_into(&mut offsets).ok()?;
        Some(Posting::with_weight_and_offsets(doc_id, positions, offsets, cursor.weight()))
    }

    fn offset_for(positions: &[u32], offsets: &[(u32, u32)], pos: u32) -> Option<(u32, u32)> {
        let idx = positions.binary_search_by_key(&pos, |p| position_start(*p)).ok()?;
        offsets.get(idx).copied()
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

/// The postings of `docs` (sorted ascending) read through `cursor`, positions included.
fn postings_of_cursor(mut cursor: TermCursor<'_>, docs: &[DocId]) -> PostingList {
    let mut items = Vec::new();
    let mut positions = Vec::new();
    for &doc in docs {
        cursor.advance_to(doc);
        match cursor.doc_id() {
            None => {
                break;
            }
            Some(found) if found == doc => {
                if cursor.positions_into(&mut positions).is_ok() {
                    items.push(Posting::with_weight(doc, positions.clone(), cursor.weight()));
                }
            }
            Some(_) => {}
        }
    }
    PostingList::from_sorted(items)
}

#[timed(search)]
pub(crate) fn phrase_matches(position_lists: &[&[u32]], offsets: &[u32]) -> bool {
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
    hits.sort_by(rank_order);
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
    let raw = (hit.raw as f32) / 1000.0;
    SearchHit {
        doc_id: hit.doc_id,
        matched_terms: hit.matched_terms,
        weight_sum: (hit.raw / 1000).min(u64::from(u32::MAX)) as u32,
        score: raw,
        base: raw,
        ..SearchHit::default()
    }
}

/// The parts of a flat query, and whether all of them must match: every part a word or a group
/// of words, phrases, exact matches and words that must all be present. `None` for any other
/// shape, and with `needs_group` for a query without synonym groups (`additive_terms` and the
/// conjunctive path take those in `best_fields`).
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
            Query::And(words) | Query::Wand(words) => {
                words.iter().all(|word| matches!(word, Query::Term(_)))
            }
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
