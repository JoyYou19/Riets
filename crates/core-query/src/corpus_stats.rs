use std::collections::{BTreeMap, HashMap};

use core_index::{
    fuzzy::{FuzzyExpansion, FuzzyOptions},
    search::SearchStats,
    types::{DocId, XPathId},
};

use crate::ast::PhraseTerm;

/// Live document count and total token length of one field.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct FieldTotals {
    pub doc_count: u64,
    pub total_doc_len: u64,
}

/// What a query leaf looks up in a field.
///
/// The document frequency BM25 uses for a leaf is the length of the posting list the leaf reads,
/// so a leaf and its statistics are always keyed the same way.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) enum LeafKey {
    Term(String),
    Prefix(String),
    Wildcard(String),
    Phrase(Vec<PhraseTerm>),
    Exact(String),
}

/// What a fuzzy leaf expands: one word of the query and how far it may be changed.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct FuzzyKey {
    word: String,
    max_edits: u8,
    prefix_length: usize,
}

impl FuzzyKey {
    pub(crate) fn new(word: &str, opts: FuzzyOptions) -> Self {
        Self {
            word: word.to_string(),
            max_edits: opts.max_edits,
            prefix_length: opts.prefix_length,
        }
    }
}

/// The BM25 statistics of one query: the totals of every field it scores and the document
/// frequency of every leaf.
///
/// Every shard reports its own values. A document lives in exactly one shard, so the sum over all
/// shards (`merge`) is what one shard holding every document would report. Scoring with that sum
/// gives a document the same score wherever it is stored.
///
/// For a fuzzy leaf the values are the words it can expand to, with the number of documents each
/// is in: the expansions that get searched are picked from all shards' words together.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CorpusStats {
    fields: HashMap<XPathId, FieldTotals>,
    doc_freqs: HashMap<XPathId, HashMap<LeafKey, u64>>,
    fuzzy: HashMap<XPathId, HashMap<FuzzyKey, BTreeMap<String, FuzzyExpansion>>>,
}

impl CorpusStats {
    pub fn new() -> Self {
        Self::default()
    }

    pub(crate) fn insert_field(&mut self, xpath: XPathId, totals: FieldTotals) {
        self.fields.insert(xpath, totals);
    }

    pub(crate) fn insert_doc_freq(&mut self, xpath: XPathId, key: LeafKey, doc_freq: u64) {
        self.doc_freqs
            .entry(xpath)
            .or_default()
            .insert(key, doc_freq);
    }

    pub(crate) fn insert_fuzzy(
        &mut self,
        xpath: XPathId,
        key: FuzzyKey,
        expansions: Vec<FuzzyExpansion>,
    ) {
        let words = self.fuzzy.entry(xpath).or_default().entry(key).or_default();
        for expansion in expansions {
            words.insert(expansion.term.clone(), expansion);
        }
    }

    /// Adds another shard's statistics to these.
    pub fn merge(&mut self, other: &CorpusStats) {
        for (&xpath, totals) in &other.fields {
            let entry = self.fields.entry(xpath).or_default();
            entry.doc_count = entry.doc_count.saturating_add(totals.doc_count);
            entry.total_doc_len = entry.total_doc_len.saturating_add(totals.total_doc_len);
        }
        for (&xpath, keys) in &other.doc_freqs {
            let field = self.doc_freqs.entry(xpath).or_default();
            for (key, &doc_freq) in keys {
                let entry = field.entry(key.clone()).or_default();
                *entry = entry.saturating_add(doc_freq);
            }
        }
        for (&xpath, keys) in &other.fuzzy {
            let field = self.fuzzy.entry(xpath).or_default();
            for (key, expansions) in keys {
                let words = field.entry(key.clone()).or_default();
                for expansion in expansions.values() {
                    match words.get_mut(&expansion.term) {
                        Some(existing) => {
                            existing.edits = existing.edits.min(expansion.edits);
                            existing.doc_freq =
                                existing.doc_freq.saturating_add(expansion.doc_freq);
                        }
                        None => {
                            words.insert(expansion.term.clone(), expansion.clone());
                        }
                    }
                }
            }
        }
    }

    pub(crate) fn fuzzy(&self, xpath: XPathId, key: &FuzzyKey) -> Option<Vec<FuzzyExpansion>> {
        self.fuzzy
            .get(&xpath)
            .and_then(|keys| keys.get(key))
            .map(|words| words.values().cloned().collect())
    }

    /// Documents containing `term` in `xpath`, summed over every shard. Every term of the query
    /// is prepared, so a term with no entry is in no document.
    pub fn term_doc_freq(&self, xpath: XPathId, term: &str) -> u32 {
        self.doc_freq(xpath, &LeafKey::Term(term.to_string()))
            .unwrap_or(0)
    }

    pub fn field(&self, xpath: XPathId) -> Option<FieldTotals> {
        self.fields.get(&xpath).copied()
    }

    /// Document frequency of a leaf, saturated to the `u32` the scorer takes; `None` when the
    /// statistics do not cover the leaf.
    pub(crate) fn doc_freq(&self, xpath: XPathId, key: &LeafKey) -> Option<u32> {
        self.doc_freqs
            .get(&xpath)
            .and_then(|keys| keys.get(key))
            .map(|&doc_freq| u32::try_from(doc_freq).unwrap_or(u32::MAX))
    }
}

/// The statistics a query is scored with: the search-wide totals where there are any, the
/// index's own otherwise. Document lengths always come from the index, they belong to its
/// documents.
pub(crate) struct ScoringStats<'a, S> {
    pub(crate) index: &'a S,
    pub(crate) corpus: Option<&'a CorpusStats>,
}

impl<S: SearchStats> SearchStats for ScoringStats<'_, S> {
    fn doc_count(&self, xpath: XPathId) -> u64 {
        match self.corpus.and_then(|corpus| corpus.field(xpath)) {
            Some(totals) => totals.doc_count,
            None => self.index.doc_count(xpath),
        }
    }

    fn total_doc_len(&self, xpath: XPathId) -> u64 {
        match self.corpus.and_then(|corpus| corpus.field(xpath)) {
            Some(totals) => totals.total_doc_len,
            None => self.index.total_doc_len(xpath),
        }
    }

    fn doc_len(&self, doc_id: DocId, xpath: XPathId) -> Option<u32> {
        self.index.doc_len(doc_id, xpath)
    }

    fn doc_range(&self) -> Option<(DocId, DocId)> {
        self.index.doc_range()
    }
}
